"""URI value object for artifact references."""
from __future__ import annotations

import dataclasses

_BUILTIN_SCHEMES: frozenset[str] = frozenset({"file", "git", "gh"})
extra_scheme_names: set[str] = set()


def _known_schemes() -> frozenset[str]:
    return _BUILTIN_SCHEMES | frozenset(extra_scheme_names)


@dataclasses.dataclass(frozen=True)
class Uri:
    scheme: str
    path: str  # everything after ://

    @classmethod
    def parse(cls, s: str) -> Uri:
        if "://" not in s:
            raise ValueError(f"invalid URI (missing '://'): {s!r}")
        scheme, path = s.split("://", 1)
        known = _known_schemes()
        if scheme not in known:
            raise ValueError(
                f"unknown scheme {scheme!r}; registered schemes: {sorted(known)}"
            )
        return cls(scheme=scheme, path=path)

    def __str__(self) -> str:
        return f"{self.scheme}://{self.path}"
