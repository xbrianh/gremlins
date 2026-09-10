from __future__ import annotations

import abc
import logging
import re
from typing import TYPE_CHECKING, Any, NamedTuple

from _gremlins_core.clients import RustClient as Client
from _gremlins_core.stages import Outcome

from gremlins.protocols import GremlinProtocol

if TYPE_CHECKING:
    from gremlins.executor.gremlin import Gremlin, State

_VAR_SUB = re.compile(r"(?<!\$)\{([-\w]+)\}")


# Also try underscore-normalized keys for hyphenated template variables
# so that {child-plan} matches bind key child_plan, etc.
def _sub_var(m: re.Match[str], subs: dict[str, str]) -> str:
    key = m.group(1)
    if key in subs:
        return subs[key]
    # Try underscore version
    alt_key = key.replace("-", "_")
    if alt_key in subs:
        return subs[alt_key]
    return m.group(0)


logger = logging.getLogger(__name__)


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


class Stage(abc.ABC):
    type: str = ""
    body: list[Stage] = []
    skip_if_exists: str = ""
    options: dict[str, Any]
    bind_map: dict[str, str]
    gremlin: GremlinProtocol | None

    def __init__(self, name: str) -> None:
        self.name = name
        self._path: str = ""
        self.client: Client | None = None
        self.client_explicit: bool = False
        self.raw_dict: dict[str, Any] | None = None
        self.options: dict[str, Any] = {}
        self.bind_map = {}
        self.gremlin = None

    def substitute_vars(
        self, text: str, state: State, extra: dict[str, str] | None = None
    ) -> str:
        """Replace {var} tokens with framework subs, resolved in: vars, and
        string options (framework wins on conflict). Unknown tokens and
        non-word braces (shell ${x}, {read:k}, brace expansion) are left as-is."""
        string_opts = {k: str(v) for k, v in self.options.items() if isinstance(v, str)}
        subs = {**string_opts, **(extra or {}), **state.framework_subs(self)}  # type: ignore[arg-type]
        # Also add hyphen-normalized variants for underscore keys so that
        # template references like {child-plan} match bind keys like child_plan
        for k, v in list(subs.items()):
            if "_" in k:
                subs.setdefault(k.replace("_", "-"), v)
        result = _VAR_SUB.sub(lambda m: _sub_var(m, subs), text)
        if result != text:
            if logger.isEnabledFor(logging.DEBUG):
                logger.debug(
                    "stage %s: substitution map has %d entries (len=%d)",
                    self.name,
                    len(subs),
                    len(text),
                )
        return result

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
        client = get_client_from_dict(d)
        stage.client = client
        stage.client_explicit = client is not None

        return stage

    @classmethod
    def orchestration_args(cls) -> list[StageInput]:
        return []

    async def run(self, gremlin: Gremlin) -> Outcome:  # noqa: ARG002
        raise NotImplementedError


# PyExec is a Rust pyclass and cannot inherit from Python's Stage.
# Register it as a virtual subclass so isinstance(Exec(...), Stage) works.
from _gremlins_core.stages import Exec  # noqa: E402

Stage.register(Exec)  # type: ignore[arg-type]
