from __future__ import annotations

import re
from typing import Any, NamedTuple

from gremlins.clients.client import Client
from gremlins.executor.state import State
from gremlins.stages.outcome import Outcome

_VAR_SUB = re.compile(r"\{(\w+)\}")


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


class StageInput(NamedTuple):
    name: str
    type: type
    required: bool
    default: Any
    help: str


class Stage:
    type: str = ""
    needs_gh: bool = False
    body: list[Stage] = []
    skip_if_exists: str = ""
    options: dict[str, Any]

    def __init__(self, name: str) -> None:
        self.name = name
        self._path: str = ""
        self.client: Client | None = None
        self.raw_dict: dict[str, Any] | None = None

    def substitute_vars(
        self, text: str, state: State, extra: dict[str, str] | None = None
    ) -> str:
        """Replace {var} tokens with framework subs, resolved in: vars, and
        string options (framework wins on conflict). Unknown tokens and
        non-word braces (shell ${x}, {read:k}, brace expansion) are left as-is."""
        string_opts = {k: v for k, v in self.options.items() if isinstance(v, str)}
        subs = {**string_opts, **(extra or {}), **state.framework_subs(self)}
        return _VAR_SUB.sub(lambda m: subs.get(m.group(1), m.group(0)), text)

    @property
    def path(self) -> str:
        return self._path

    @path.setter
    def path(self, value: str) -> None:
        self._path = value
        for c in getattr(self, "body", []):
            c.path = f"{value}/{c.name}"

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> Stage:
        stage = cls(d["name"], d.get("prompt") or [], d.get("options") or {})  # type: ignore[call-arg]
        stage.client = get_client_from_dict(d)
        return stage

    @classmethod
    def orchestration_args(cls) -> list[StageInput]:
        return []

    async def run(self, state: State) -> Outcome:  # noqa: ARG002
        raise NotImplementedError
