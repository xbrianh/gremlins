from __future__ import annotations

from typing import TYPE_CHECKING, Any, Protocol

if TYPE_CHECKING:
    from gremlins.executor.state import State
    from gremlins.stages.outcome import Outcome


class GremlinProtocol(Protocol):
    registry: Any

    async def fork(
        self,
        state: State,
        target_id: str,
        *,
        parent_id: str = "",
        group_name: str = "",
        child_key: str = "",
        pipeline: Any = None,
    ) -> State:
        ...


class StageProtocol(Protocol):
    name: str
    type: str
    skip_if_exists: str
    out_map: dict[str, str]
    gremlin: GremlinProtocol | None

    async def run(self, state: State) -> Outcome:
        ...
