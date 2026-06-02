"""Protocol definitions for dependency inversion between stages and executor."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Protocol, runtime_checkable

if TYPE_CHECKING:
    from gremlins.executor.state import State
    from gremlins.stages.outcome import Outcome


@runtime_checkable
class GremlinProtocol(Protocol):
    """Protocol for Gremlin interface used by stages."""

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
        """Fork the gremlin for a parallel child."""
        ...


@runtime_checkable
class StageProtocol(Protocol):
    """Protocol for Stage interface used by executor."""

    name: str
    type: str
    skip_if_exists: str
    out_map: dict[str, str]
    gremlin: GremlinProtocol | None

    async def run(self, state: State) -> Outcome:
        """Execute the stage."""
        ...
