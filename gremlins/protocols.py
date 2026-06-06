"""Protocol definitions for gremlins/stages/* and gremlins/executor/* to avoid circular imports."""

from __future__ import annotations

from typing import Any, Protocol


class GremlinShim:
    """Minimal wrapper exposing state and registry for stage run() signatures.

    Used to adapt State to the Gremlin protocol when a full Gremlin instance
    is not available (e.g., in child processes or transient contexts).
    """

    def __init__(self, state: Any) -> None:
        self.state = state
        self.registry = state.artifacts


class GremlinProtocol(Protocol):
    """What stages need from a Gremlin."""

    state: Any
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

    async def run(self, gremlin: Any) -> Any:
        """Run this stage with the orchestrator."""
        ...
