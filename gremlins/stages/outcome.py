from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Done:
    pass


@dataclass(frozen=True)
class NeedsFix:
    detail: str
    returncode: int | None = None


class Bail(Exception):
    def __init__(self, reason: str) -> None:
        super().__init__(reason)
        self.reason = reason


Outcome = Done | NeedsFix
