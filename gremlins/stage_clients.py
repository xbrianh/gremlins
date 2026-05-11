from __future__ import annotations

import json
from collections.abc import Sequence
from typing import TYPE_CHECKING

from gremlins.clients.client import PACKAGE_DEFAULT, Client
from gremlins.state import resolve_state_file

if TYPE_CHECKING:
    from gremlins.pipeline import Pipeline
    from gremlins.stages.base import Stage


def resolve_stage_client(
    stage_client: Client | None,
    cli: Client | None,
    pipeline_default: Client | None,
) -> Client:
    return stage_client or cli or pipeline_default or PACKAGE_DEFAULT


def collect_stage_specs(
    pipeline: Pipeline,
    cli_spec: Client | None,
) -> dict[str, Client]:
    specs: dict[str, Client] = {}

    def _walk(entries: list[Stage]) -> None:
        for e in entries:
            entry_client = None if e.type == "parallel" else e.client
            specs[e.name] = resolve_stage_client(
                entry_client, cli_spec, pipeline.default_client
            )
            if e.body:
                _walk(e.body)

    _walk(list(pipeline.stages))
    return specs


def load_stage_specs_from_state(gr_id: str | None) -> dict[str, Client]:
    if not gr_id:
        return {}
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return {}
    data = json.loads(sf.read_text(encoding="utf-8"))
    stored = data.get("stage_clients", {})
    return {str(k): Client.parse(str(v)) for k, v in stored.items()}


def _format_missing_stage_specs(names: Sequence[str]) -> str:
    missing = ", ".join(repr(name) for name in sorted(names))
    suffix = "" if len(names) == 1 else "s"
    return f"stage_clients missing stage{suffix}: {missing}"


def validate_stage_specs(stage_specs: dict[str, Client], pipeline: Pipeline) -> None:
    expected_stage_names: set[str] = set()

    def _walk(entries: list[Stage]) -> None:
        for entry in entries:
            expected_stage_names.add(entry.name)
            if entry.body:
                _walk(entry.body)

    _walk(list(pipeline.stages))

    missing_stage_names = sorted(expected_stage_names.difference(stage_specs))
    if missing_stage_names:
        raise ValueError(_format_missing_stage_specs(missing_stage_names))


def require_stage_spec(
    stage_specs: dict[str, Client],
    name: str,
) -> Client:
    try:
        return stage_specs[name]
    except KeyError as exc:
        raise ValueError(_format_missing_stage_specs([name])) from exc
