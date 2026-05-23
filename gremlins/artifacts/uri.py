"""URI value object for artifact references."""

from __future__ import annotations

import dataclasses

_BUILTIN_SCHEMES: frozenset[str] = frozenset({"file", "git", "gh"})


@dataclasses.dataclass(frozen=True)
class Uri:
    scheme: str
    path: str  # everything after ://

    @classmethod
    def parse(cls, s: str) -> Uri:
        if "://" not in s:
            raise ValueError(f"invalid URI (missing '://'): {s!r}")
        scheme, path = s.split("://", 1)
        if scheme not in _BUILTIN_SCHEMES:
            raise ValueError(
                f"unknown scheme {scheme!r}; known schemes: {sorted(_BUILTIN_SCHEMES)}"
            )
        return cls(scheme=scheme, path=path)

    def __str__(self) -> str:
        return f"{self.scheme}://{self.path}"
