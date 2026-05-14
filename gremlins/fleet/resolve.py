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


def collect_gremlin_matches(
    target: str,
) -> tuple[list[tuple[str, str, str]], tuple[str, str, str] | None]:
    """Return (all_substring_matches, exact_match_or_none) for target."""
    matches = [(gid, sf, wdir) for gid, sf, wdir in iter_state_files() if target in gid]
    exact = next((m for m in matches if m[0] == target), None)
    return matches, exact


def resolve_gremlin(target: str) -> tuple[str, str, str] | None:
    """Resolve id substring to a single (gremlin_id, sf, wdir) or print error and return None.

    An exact id match always wins over substring matches.
    """
    matches, exact = collect_gremlin_matches(target)
    if not matches:
        print(f"no gremlin matched: {target}")
        return None
    if exact is not None:
        return exact
    if len(matches) > 1:
        print(
            f"ambiguous id '{target}' matched {len(matches)} gremlins — use a longer prefix:"
        )
        for gremlin_id, _, _ in matches:
            print(f"  {gremlin_id}")
        return None
    return matches[0]
