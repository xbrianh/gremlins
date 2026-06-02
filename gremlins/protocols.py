"""Protocol definitions for gremlins/stages/* and gremlins/executor/* to avoid circular imports."""

from __future__ import annotations

from typing import Any, Protocol


class GremlinProtocol(Protocol):
    """What stages need from a Gremlin."""

    registry: Any

    async def fork(
        self,
        state: Any,
        target_id: str,
        *,
        parent_id: str = "",
        group_name: str = "",
        child_key: str = "",
        pipeline: Any | None = None,
    ) -> Any:
        """Fork a new child Gremlin from the current state."""
        ...


class StageProtocol(Protocol):
    """What executor/* needs from a stage."""

    name: str
    type: str
    path: Any
    body: Any
    out_map: dict[str, str]
    gremlin: GremlinProtocol | None
    client: Any
    skip_if_exists: str

    async def run(self, state: Any) -> Any:
        """Run this stage with the given execution state."""
        ...
