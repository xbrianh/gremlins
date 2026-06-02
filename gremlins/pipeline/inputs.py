"""Input source declarations for pipelines."""

from __future__ import annotations

import dataclasses
from typing import Any, cast


@dataclasses.dataclass
class InputSource:
    """A single input source declaration."""

    name: str
    types: list[str]
    optional: bool = False

    def __post_init__(self) -> None:
        if not self.types:
            raise ValueError(
                f"input source {self.name!r}: types list must not be empty"
            )
        valid_types = {"filepath", "string"}
        for t in self.types:
            if t not in valid_types:
                raise ValueError(
                    f"input source {self.name!r}: unknown type {t!r}. "
                    f"Supported types: {', '.join(sorted(valid_types))}"
                )


class InputSources:
    """Container for input source declarations from a pipeline's inputs: block."""

    def __init__(self, sources: dict[str, InputSource] | None = None) -> None:
        self.sources = sources or {}

    @classmethod
    def from_yaml(cls, raw: dict[str, Any]) -> InputSources:
        """Parse sources: block from YAML."""
        allowed_keys = {"plan", "issue", "instructions"}
        sources: dict[str, InputSource] = {}
        for key, entry in raw.items():
            if key not in allowed_keys:
                raise ValueError(
                    f"input source {key!r}: unrecognized key. "
                    f"Allowed keys: {', '.join(sorted(allowed_keys))}"
                )
            if not isinstance(entry, dict):
                raise ValueError(
                    f"input source {key!r}: expected a mapping, got {type(entry).__name__}"
                )
            entry = cast(dict[str, Any], entry)

            # Parse type field: can be a string or list of strings
            type_raw = entry.get("type")
            if type_raw is None:
                raise ValueError(f"input source {key!r}: missing required 'type' field")

            if isinstance(type_raw, str):
                types = [type_raw]
            elif isinstance(type_raw, list):
                type_raw = cast(list[Any], type_raw)
                if not type_raw:
                    raise ValueError(
                        f"input source {key!r}: type list must not be empty"
                    )
                for t in type_raw:
                    if not isinstance(t, str):
                        raise ValueError(
                            f"input source {key!r}: all type entries must be strings"
                        )
                types = cast(list[str], type_raw)
            else:
                raise ValueError(
                    f"input source {key!r}: 'type' must be a string or list of strings, "
                    f"got {type(type_raw).__name__}"
                )

            optional = bool(entry.get("optional", False))
            sources[key] = InputSource(name=key, types=types, optional=optional)

        return cls(sources)

    def get(self, key: str) -> InputSource | None:
        """Retrieve a source by name, or None if not defined."""
        return self.sources.get(key)
