"""Per-group worktree mirror and attempt tracking for parallel execution."""

from __future__ import annotations

import dataclasses
import json
import logging
import pathlib
from typing import Any

from gremlins.executor.state import StateData, resolve_state_file

logger = logging.getLogger(__name__)


@dataclasses.dataclass
class ParallelGroupState:
    """Owns the state.json slice for one parallel group."""

    group_name: str
    parent_data: StateData
    worktree_paths: dict[str, pathlib.Path] = dataclasses.field(
        default_factory=dict[str, pathlib.Path]
    )
    base_head: str = ""

    def hydrate(self) -> None:
        if self.worktree_paths:
            return
        sf = resolve_state_file(self.parent_data.gremlin_id)
        if sf is None or not sf.exists():
            return
        try:
            data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
            parallel_worktrees: dict[str, Any] = data.get("parallel_worktrees") or {}
            entry: dict[str, Any] = parallel_worktrees.get(self.group_name) or {}
            paths: dict[str, str] = entry.get("paths") or {}
            for k, v in paths.items():
                self.worktree_paths[k] = pathlib.Path(v)
            self.base_head = entry.get("base_head", "") or self.base_head
        except Exception as exc:
            logger.warning(
                "parallel group %r: could not hydrate worktree paths: %s",
                self.group_name,
                exc,
            )

    def persist(self) -> None:
        self.parent_data.patch_parallel_worktrees(
            self.group_name,
            base_head=self.base_head,
            paths={k: str(v) for k, v in self.worktree_paths.items()},
        )

    def clear(self) -> None:
        self.parent_data.patch_parallel_worktrees(
            self.group_name, base_head=None, paths=None
        )

    def record_attempt(self, child_key: str, attempt: str) -> None:
        self.parent_data.patch_parallel_attempt(child_key, attempt)

    def clear_attempts(self) -> None:
        self.parent_data.patch(_delete=("parallel_attempts",))

    def write_bail(self, child_key: str, reason: str) -> None:
        sf = resolve_state_file(self.parent_data.gremlin_id)
        if sf is None or not sf.exists():
            return
        try:
            pa: dict[str, Any] = (
                json.loads(sf.read_text(encoding="utf-8")).get("parallel_attempts")
                or {}
            )
            self.parent_data.write_bail_file(
                "other", reason, attempt=pa.get(child_key) or ""
            )
        except Exception:
            pass

    def collect_bails(self, child_keys: list[str]) -> tuple[list[str], dict[str, str]]:
        sf = resolve_state_file(self.parent_data.gremlin_id)
        if sf is None or not sf.exists():
            return [], {}
        try:
            data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
            parallel_attempts: dict[str, str] = data.get("parallel_attempts") or {}
            bailed: list[str] = []
            first_bail: dict[str, str] = {}
            for key in child_keys:
                child_attempt = parallel_attempts.get(key) or ""
                bail_file = sf.parent / f"bail_{child_attempt}.json"
                if child_attempt and bail_file.exists():
                    bailed.append(key)
                    if not first_bail:
                        try:
                            first_bail = dict(
                                json.loads(bail_file.read_text(encoding="utf-8"))
                            )
                        except Exception:
                            first_bail = {"class": "other"}
            return bailed, first_bail
        except RuntimeError:
            raise
        except Exception as exc:
            logger.warning("fan-in bail aggregation failed: %s", exc)
            return [], {}
