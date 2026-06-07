"""Protocol definitions for gremlins/stages/* and gremlins/executor/* to avoid circular imports."""

from __future__ import annotations

from typing import Any, Protocol


class _StateProtocol(Protocol):
    """What stages need from a State object."""

    data: Any
    artifacts: Any
    cwd: str
    artifact_dir: Any
    base_ref: str
    client: Any
    repo: str
    parent_stage: str
    worktree: Any
    worktree_parent: Any

    def done_for(self, path: str) -> set[str]: ...

    def mark_done(self, path: str, child_name: str) -> None: ...

    def record_bail(self, reason: str, *, kind: str = "other") -> None: ...

    def record_stage_progress(
        self, name: str, sub_stage: object = None, *, parent_stage: str = ""
    ) -> None: ...

    def record_state_field(self, **fields: Any) -> None: ...

    def format(self, template: str) -> str: ...

    def make_runner(self, entry: Any, gremlin: Any, scope: Any | None = None, *, record_stage: bool = True) -> Any: ...


class GremlinProtocol(Protocol):
    """What stages need from a Gremlin."""

    state: _StateProtocol | None
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
