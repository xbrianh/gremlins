"""Shared helpers for composite stages (Loop, Sequence, Parallel)."""

from __future__ import annotations

import dataclasses
import pathlib
from typing import TYPE_CHECKING, Any

from _gremlins_core.clients import Client
from _gremlins_core.config import scratch_root

from gremlins.protocols import GremlinProtocol

if TYPE_CHECKING:
    from gremlins.executor.gremlin import State


def get_client_from_dict(d: dict[str, Any]) -> Client | None:
    raw = d.get("client")
    if raw is None:
        return None
    if not isinstance(raw, str):
        name = d.get("name") or d.get("type") or "?"
        raise ValueError(
            f"stage {name!r}: 'client' must be a string, got {type(raw)!r}"
        )
    return Client.parse(raw)


class StageAttrs:
    """Common attributes shared by composite stages and duck-typed test stages."""

    type: str = ""
    body: list[Any] = []
    skip_if_exists: str = ""

    def __init__(self, name: str) -> None:
        self.name = name
        self._path: str = ""
        self.client: Client | None = None
        self.client_explicit: bool = False
        self.raw_dict: dict[str, Any] | None = None
        self.options: dict[str, Any] = {}
        self.bind_map: dict[str, str] = {}
        self.gremlin: GremlinProtocol | None = None

    @property
    def path(self) -> str:
        return self._path

    @path.setter
    def path(self, value: str) -> None:
        self._path = value
        for c in getattr(self, "body", []):
            c.path = f"{value}/{c.name}"


def child_state(
    parent: State, child: Any, *, fan_out: bool = False, child_id: str | None = None
) -> State:
    """Derive a child State from parent."""
    client = (
        child.client
        if (child.client is not None and child.client_explicit)
        else parent.client
    )

    if not fan_out:
        new_state = dataclasses.replace(parent, client=client)
        if str(client) != new_state.data.client:
            new_state.data.patch(client=str(client))
        return new_state
    if child_id:
        artifact_dir = pathlib.Path(scratch_root(child_id)) / "artifacts"
        artifact_dir.mkdir(parents=True, exist_ok=True)
    else:
        artifact_dir = parent.artifact_dir / child.name
        artifact_dir.mkdir(parents=True, exist_ok=True)
    return dataclasses.replace(
        parent,
        client=client,
        artifact_dir=artifact_dir,
        child_key=child.name,
    )
