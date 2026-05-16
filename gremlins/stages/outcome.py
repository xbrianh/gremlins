from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Done:
    pass


@dataclass(frozen=True)
class NeedsFix:
    detail: str
    returncode: int | None = None


@dataclass(frozen=True)
class Bail:
    reason: str


Outcome = Done | NeedsFix | Bail
