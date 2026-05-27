"""Resolve in: map entries against the artifact registry."""

from __future__ import annotations

from gremlins.artifacts.registry import ArtifactRegistry, MissingArtifact
from gremlins.utils.text import to_str


def resolve_in_map(
    artifacts: ArtifactRegistry, in_map: dict[str, str]
) -> dict[str, str]:
    result: dict[str, str] = {}
    for var, raw_path in in_map.items():
        default: str | None = None
        path = raw_path
        if "?" in raw_path:
            path, default = raw_path.split("?", 1)
        parts = path.split(".")
        if any(not p for p in parts):
            raise ValueError(f"in: path {path!r} has empty segment")
        key, *attrs = parts
        try:
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
        except MissingArtifact:
            if default is None:
                raise
            result[var] = default
    return result
