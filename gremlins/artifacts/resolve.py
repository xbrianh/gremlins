"""Resolve in: map entries against the artifact registry."""

from __future__ import annotations

from gremlins.artifacts.registry import ArtifactRegistry, MissingArtifact
from gremlins.utils.text import to_str


def resolve_in_map(
    artifacts: ArtifactRegistry, in_map: dict[str, str]
) -> dict[str, str]:
    result: dict[str, str] = {}
    for var, raw_path in in_map.items():
        path, sep, default = raw_path.partition("?")
        parts = path.split(".")
        if any(not p for p in parts):
            raise ValueError(f"in: path {path!r} has empty segment")
        key, *attrs = parts
        try:
            value = artifacts.read(key)
        except MissingArtifact:
            if not sep:
                raise
            result[var] = default
            continue
        missing = False
        for attr in attrs:
            if attr.startswith("_"):
                raise ValueError(
                    f"in: path {path!r}: private attribute {attr!r} not accessible"
                )
            try:
                value = value[attr]
            except (KeyError, TypeError):
                if not sep:
                    raise ValueError(f"in: path {path!r}: value has no key {attr!r}")
                result[var] = default
                missing = True
                break
        if not missing:
            result[var] = to_str(value)
    return result
