from __future__ import annotations

import dataclasses
import json
from collections.abc import Sequence
from typing import TYPE_CHECKING

from gremlins.clients.registry import CLIENT_FACTORIES
from gremlins.state import resolve_state_file

if TYPE_CHECKING:
    from gremlins.pipeline import PipelineDef, StageEntry


@dataclasses.dataclass(frozen=True)
class ClientSpec:
    provider: str
    model: str

    @staticmethod
    def parse(spec: str) -> ClientSpec:
        if ":" not in spec:
            raise ValueError(
                f"invalid client specifier {spec!r}: expected 'provider:model'"
            )
        provider, _, model = spec.partition(":")
        if not provider:
            raise ValueError(
                f"invalid client specifier {spec!r}: expected 'provider:model'"
            )
        if not model:
            raise ValueError(
                f"invalid client specifier {spec!r}: model must not be empty"
            )
        if provider not in CLIENT_FACTORIES:
            raise ValueError(
                f"unknown provider {provider!r} in client specifier {spec!r}"
            )
        return ClientSpec(provider=provider, model=model)

    def __str__(self) -> str:
        return f"{self.provider}:{self.model}"


PACKAGE_DEFAULT = ClientSpec("claude", "sonnet")


def resolve_stage_client(
    stage_client: ClientSpec | None,
    cli: ClientSpec | None,
    pipeline_default: ClientSpec | None,
) -> ClientSpec:
    return stage_client or cli or pipeline_default or PACKAGE_DEFAULT


def collect_stage_specs(
    pipeline: PipelineDef,
    cli_spec: ClientSpec | None,
) -> dict[str, ClientSpec]:
    specs: dict[str, ClientSpec] = {}

    def _walk(entries: list[StageEntry]) -> None:
        for e in entries:
            entry_client = None if e.type == "parallel" else e.client
            specs[e.name] = resolve_stage_client(
                entry_client, cli_spec, pipeline.default_client
            )
            if e.body:
                _walk(e.body)

    _walk(list(pipeline.stages))
    return specs


def load_stage_specs_from_state(gr_id: str | None) -> dict[str, ClientSpec]:
    """Return the stage→spec map persisted in state.json, or {} if absent.

    Raises ValueError if stage_clients exists but contains an unparseable spec,
    or OSError/json.JSONDecodeError if the state file is malformed.
    """
    if not gr_id:
        return {}
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return {}
    data = json.loads(sf.read_text(encoding="utf-8"))
    stored = data.get("stage_clients", {})
    return {str(k): ClientSpec.parse(str(v)) for k, v in stored.items()}


def _format_missing_stage_specs(names: Sequence[str]) -> str:
    missing = ", ".join(repr(name) for name in sorted(names))
    suffix = "" if len(names) == 1 else "s"
    return f"stage_clients missing stage{suffix}: {missing}"


def validate_stage_specs(
    stage_specs: dict[str, ClientSpec], pipeline: PipelineDef
) -> None:
    expected_stage_names: set[str] = set()

    def _walk(entries: list[StageEntry]) -> None:
        for entry in entries:
            expected_stage_names.add(entry.name)
            if entry.body:
                _walk(entry.body)

    _walk(list(pipeline.stages))

    missing_stage_names = sorted(expected_stage_names.difference(stage_specs))
    if missing_stage_names:
        raise ValueError(_format_missing_stage_specs(missing_stage_names))


def require_stage_spec(
    stage_specs: dict[str, ClientSpec],
    name: str,
) -> ClientSpec:
    try:
        return stage_specs[name]
    except KeyError as exc:
        raise ValueError(_format_missing_stage_specs([name])) from exc
