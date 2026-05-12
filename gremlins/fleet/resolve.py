"""Gremlin resolution by id prefix."""

import pathlib
from typing import Any

from gremlins.fleet.state import iter_state_files
from gremlins.utils.yaml_io import YamlLoadError


def stage_names_for_gremlin(state: dict[str, Any]) -> list[str]:
    pipeline_path = state.get("pipeline_path")
    if pipeline_path:
        try:
            from gremlins.pipeline import Pipeline

            pipeline = Pipeline.from_yaml(pathlib.Path(str(pipeline_path)))
            return [s.name for s in pipeline.stages]
        except (FileNotFoundError, ValueError, YamlLoadError):
            pass
    return []


def resolve_gremlin(target: str) -> tuple[str, str, str] | None:
    """Resolve id prefix to a single (gr_id, sf, wdir) or print error and return None."""
    matches: list[tuple[str, str, str]] = []
    for gr_id, sf, wdir in iter_state_files():
        if target in gr_id:
            matches.append((gr_id, sf, wdir))
    if not matches:
        print(f"no gremlin matched: {target}")
        return None
    if len(matches) > 1:
        print(
            f"ambiguous id '{target}' matched {len(matches)} gremlins — use a longer prefix:"
        )
        for gr_id, _, _ in matches:
            print(f"  {gr_id}")
        return None
    return matches[0]
