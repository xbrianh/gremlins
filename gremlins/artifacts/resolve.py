"""Resolve in: map entries against the artifact registry, supporting dotted attribute paths."""

from __future__ import annotations

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.utils.text import to_str


def resolve_in_map(
    artifacts: ArtifactRegistry, in_map: dict[str, str]
) -> dict[str, str]:
    result: dict[str, str] = {}
    for var, path in in_map.items():
        parts = path.split(".")
        if any(not p for p in parts):
            raise ValueError(f"in: path {path!r} has empty segment")
        key, *attrs = parts
        value = artifacts.read(key)
        for attr in attrs:
            if attr.startswith("_"):
                raise ValueError(
                    f"in: path {path!r}: private attribute {attr!r} not accessible"
                )
            try:
                value = getattr(value, attr)
            except AttributeError:
                raise ValueError(
                    f"in: path {path!r}: {type(value).__name__} has no attribute {attr!r}"
                )
        result[var] = to_str(value)
    return result
