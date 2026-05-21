"""Pure helpers for scanning per-child bail files and deciding group bail policy."""

from __future__ import annotations

import dataclasses
import json
import pathlib


@dataclasses.dataclass
class BailedChild:
    key: str
    bail: dict[str, str]


@dataclasses.dataclass
class BailDecision:
    should_bail: bool
    first_bail: dict[str, str]


def collect_bails(
    state_dir: pathlib.Path,
    child_keys: list[str],
    parallel_attempts: dict[str, str],
) -> list[BailedChild]:
    result: list[BailedChild] = []
    for key in child_keys:
        attempt = parallel_attempts.get(key) or ""
        if not attempt:
            continue
        bail_file = state_dir / f"bail_{attempt}.json"
        if not bail_file.exists():
            continue
        try:
            bail = dict(json.loads(bail_file.read_text(encoding="utf-8")))
        except Exception:
            bail = {"class": "other"}
        result.append(BailedChild(key=key, bail=bail))
    return result


def decide(bailed: list[BailedChild], total: int, policy: str) -> BailDecision:
    if policy == "any":
        should_bail = bool(bailed)
    elif policy == "all":
        should_bail = bool(bailed) and len(bailed) == total
    else:
        raise ValueError(f"unknown bail_policy {policy!r}")
    first_bail = bailed[0].bail if bailed else {}
    return BailDecision(should_bail=should_bail, first_bail=first_bail)
