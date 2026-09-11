"""Execution context and state.json I/O for gremlin pipelines."""

from __future__ import annotations

import argparse
import copy
import dataclasses
import datetime
import json
import logging
import math
import os
import pathlib
import secrets
from collections.abc import Callable, Sequence
from typing import TYPE_CHECKING, Any, ClassVar

from _gremlins_core.artifacts import ArtifactRegistry
from _gremlins_core.clients import RustClient as Client
from _gremlins_core.config import project_root, state_root
from _gremlins_core.stages import FRAMEWORK_KEYS, Done

from gremlins.utils.state_file import locked_update

if TYPE_CHECKING:
    from _gremlins_core.schemas import Pipeline

    from gremlins.executor.gremlin import Gremlin

from gremlins.protocols import StageProtocol

logger = logging.getLogger(__name__)


def resolve_state_file(gremlin_id: str | None) -> pathlib.Path | None:
    """Return path to state.json for gremlin_id, or None when gremlin_id is absent."""
    if not gremlin_id:
        return None
    return pathlib.Path(state_root()) / gremlin_id / "state.json"


def write_state(state_dir: pathlib.Path, data: dict[str, Any]) -> None:
    """Atomically overwrite state.json (no merge)."""
    sf = state_dir / "state.json"
    tmp = state_dir / f"state.json.{os.getpid()}.{secrets.token_hex(4)}.tmp"
    tmp.write_text(json.dumps(data), encoding="utf-8")
    os.replace(tmp, sf)


def _stage_list() -> list[StageProtocol]:
    return []


def read_state_json(sf: pathlib.Path | None) -> dict[str, Any]:
    if sf is None or not sf.exists():
        return {}
    try:
        return json.loads(sf.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return {}


class StateData:
    """Handle for reading/writing state.json. All field reads go to disk."""

    FIELD_DEFAULTS: ClassVar[dict[str, Any]] = {
        "attempt": "",
        "kind": "",
        "project_root": "",
        "workdir": "",
        "setup_kind": "",
        "worktree_base": "",
        "status": "",
        "started_at": "",
        "description": "",
        "parent_id": "",
        "pipeline_args": [],
        "client": "",
        "pipeline_path": "",
        "stage": "",
        "pid": None,
        "stage_inputs": {},
        "group_name": "",
        "child_key": "",
        "exit_code": None,
    }

    def __init__(self, gremlin_id: str | None = None) -> None:
        self.gremlin_id = gremlin_id
        self.state_file = resolve_state_file(gremlin_id)
        self._cache: dict[str, Any] | None = None

    def __getattr__(self, name: str) -> Any:
        if name not in self.FIELD_DEFAULTS:
            raise AttributeError(f"{type(self).__name__!r} has no field {name!r}")
        if self._cache is None:
            self._cache = read_state_json(self.state_file)
        if name not in self._cache:
            default: object = self.FIELD_DEFAULTS[name]
            if isinstance(default, list):
                return list(default)  # type: ignore[arg-type]
            if isinstance(default, dict):
                return dict(default)  # type: ignore[arg-type]
            return default
        value: object = self._cache[name]
        if isinstance(value, list):
            return list(value)  # type: ignore[arg-type]
        if isinstance(value, dict):
            return dict(value)  # type: ignore[arg-type]
        return value

    def __setattr__(self, name: str, value: Any) -> None:
        if name in ("gremlin_id", "state_file", "_cache"):
            super().__setattr__(name, value)
        else:
            raise TypeError(
                f"StateData fields must be modified via patch(), "
                f"not direct assignment (attempted setattr {name!r})"
            )

    def persist(self, state_dir: pathlib.Path, data: dict[str, Any]) -> None:
        if not self.gremlin_id:
            raise ValueError("cannot persist StateData with no gremlin_id")
        data["id"] = self.gremlin_id
        write_state(state_dir, data)
        self.state_file = state_dir / "state.json"
        self._cache = None

    def patch(self, _delete: tuple[str, ...] = (), **fields: object) -> None:
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists():
            return
        try:

            def _apply(data: dict[str, Any]) -> None:
                for key in _delete:
                    data.pop(key, None)
                data.update(fields)

            locked_update(sf, _apply)
        except Exception:
            pass
        self._cache = None

    def read_str(self, field: str) -> str:
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists():
            return ""
        try:
            return json.loads(sf.read_text(encoding="utf-8")).get(field) or ""
        except Exception:
            return ""

    def set_stage(
        self, stage: str, sub_stage: object = None, *, parent_stage: str = ""
    ) -> None:
        try:
            target_stage = parent_stage if parent_stage else stage
            target_sub = stage if parent_stage else sub_stage
            if not target_stage or not self.gremlin_id:
                return
            now = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
            if target_sub is not None:
                self.patch(
                    stage=target_stage, stage_updated_at=now, sub_stage=target_sub
                )
            else:
                self.patch(
                    _delete=("sub_stage",), stage=target_stage, stage_updated_at=now
                )
        except Exception:
            pass

    def write_bail_file(self, bail_class: str, bail_detail: str = "") -> None:
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists() or not bail_class:
            return
        # Read attempt directly from disk to avoid stale _cache issues
        # when another process has patched state.json concurrently.
        attempt = read_state_json(sf).get("attempt") or ""
        if not attempt:
            return
        try:
            state_dir = sf.parent
            bail_path = state_dir / f"bail_{attempt}.json"
            if bail_path.exists():
                return
            payload = json.dumps(
                {
                    "class": bail_class,
                    "detail": bail_detail,
                    "ts": datetime.datetime.now(datetime.UTC).isoformat(),
                },
                ensure_ascii=False,
            )
            tmp = state_dir / f".bail_{attempt}_{secrets.token_hex(4)}.tmp"
            tmp.write_text(payload, encoding="utf-8")
            tmp.rename(bail_path)
        except Exception:
            pass

    def accumulate_token_usage(self, usage: dict[str, int]) -> None:
        """Fold a per-run token-usage delta into the cumulative state.json total.

        No-ops without a state file. Never raises — telemetry bookkeeping must
        not crash a running gremlin.
        """
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists() or not usage:
            return
        try:

            def _apply(data: dict[str, Any]) -> None:
                total: dict[str, int] = dict(data.get("token_usage") or {})
                for key, val in usage.items():
                    total[key] = int(total.get(key, 0)) + int(val)
                data["token_usage"] = total

            locked_update(sf, _apply)
        except Exception:
            pass

    def read_bail_info(self) -> dict[str, str] | None:
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists():
            return None
        try:
            data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
            attempt = data.get("attempt") or ""
            if not attempt:
                return None
            bail_path = sf.parent / f"bail_{attempt}.json"
            if not bail_path.exists():
                return None
            return dict(json.loads(bail_path.read_text(encoding="utf-8")))
        except Exception:
            return None

    def patch_parallel_worktrees(
        self,
        group_name: str,
        *,
        base_head: str | None,
        paths: dict[str, str] | None,
    ) -> None:
        if not self.gremlin_id or not group_name:
            return
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists():
            return
        try:

            def _apply(data: dict[str, Any]) -> None:
                groups: dict[str, Any] = dict(data.get("parallel_worktrees") or {})
                if base_head is None and paths is None:
                    groups.pop(group_name, None)
                else:
                    groups[group_name] = {
                        "base_head": base_head or "",
                        "paths": dict(paths or {}),
                    }
                if groups:
                    data["parallel_worktrees"] = groups
                else:
                    data.pop("parallel_worktrees", None)

            locked_update(sf, _apply)
        except Exception:
            pass

    def done_for(self, path: str) -> set[str]:
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists():
            return set()
        try:
            data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
            dc: dict[str, Any] = data.get("done_children") or {}
            children: list[str] = list(dc.get(path) or [])
            return set(children)
        except Exception:
            return set()

    def mark_done(self, path: str, child_name: str) -> None:
        if not self.gremlin_id or not path:
            return
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists():
            return
        try:

            def _mark(data: dict[str, Any]) -> None:
                dc: dict[str, list[str]] = dict(data.get("done_children") or {})
                existing = list(dc.get(path) or [])
                if child_name not in existing:
                    existing.append(child_name)
                dc[path] = existing
                data["done_children"] = dc

            locked_update(sf, _mark)
        except Exception:
            pass

    def clear_done(self, path: str) -> None:
        if not self.gremlin_id or not path:
            return
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists():
            return
        try:

            def _clear(data: dict[str, Any]) -> None:
                dc: dict[str, list[str]] = dict(data.get("done_children") or {})
                dc.pop(path, None)
                if dc:
                    data["done_children"] = dc
                else:
                    data.pop("done_children", None)

            locked_update(sf, _clear)
        except Exception:
            pass

    def add_subprocess_cost(self, amount: float) -> None:
        if not amount or not self.gremlin_id:
            return
        if not math.isfinite(amount) or amount < 0:
            return
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists():
            return
        try:

            def _apply(data: dict[str, Any]) -> None:
                try:
                    current = float(data.get("subprocess_cost_usd") or 0.0)
                except (ValueError, TypeError):
                    current = 0.0
                data["subprocess_cost_usd"] = current + amount

            locked_update(sf, _apply)
        except Exception:
            pass

    def patch_parallel_attempt(self, child_key: str, attempt: str) -> None:
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists() or not attempt:
            return
        try:

            def _apply(data: dict[str, Any]) -> None:
                pa: dict[str, Any] = dict(data.get("parallel_attempts") or {})
                pa[child_key] = attempt
                data["parallel_attempts"] = pa

            locked_update(sf, _apply)
        except Exception:
            pass

    def write_terminal_state(self, exit_code: int) -> None:
        if not self.gremlin_id:
            return
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None:
            return
        state_dir = sf.parent
        try:
            (state_dir / "finished").touch()
        except OSError:
            pass
        now_iso = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
        status = "done" if exit_code == 0 else "stopped"
        try:
            self.patch(status=status, ended_at=now_iso, exit_code=exit_code)
        except Exception:
            pass


@dataclasses.dataclass
class State:
    data: StateData
    client: Client
    artifact_dir: pathlib.Path
    artifacts: ArtifactRegistry
    cwd: str = ""
    args: argparse.Namespace = dataclasses.field(default_factory=argparse.Namespace)
    pipeline_data: Pipeline | None = None
    current_scope: list[StageProtocol] = dataclasses.field(default_factory=_stage_list)
    child_key: str | None = None
    parent_stage: str = ""
    worktree: pathlib.Path | None = None
    worktree_parent: pathlib.Path | None = None
    base_ref: str = ""

    FRAMEWORK_KEYS: ClassVar[frozenset[str]] = FRAMEWORK_KEYS
    loop_stack: list[tuple[str, int]] = dataclasses.field(default_factory=list)

    @property
    def loop_iter(self) -> str:
        """Name-qualified iteration path.

        Format is ``name~iteration`` per frame, joined with ``~``.
        A nested loop yields e.g. ``outer~2~inner~1``.
        Returns ``"1"`` when no loop is active.
        """
        if not self.loop_stack:
            return "1"
        return "~".join(f"{name}~{n}" for name, n in self.loop_stack)

    def push_loop(self, stage_path: str) -> None:
        self.loop_stack.append((stage_path.replace("/", "-"), 1))

    def pop_loop(self) -> None:
        if self.loop_stack:
            self.loop_stack.pop()

    def set_loop_iteration(self, n: int) -> None:
        if self.loop_stack:
            name, _ = self.loop_stack[-1]
            self.loop_stack[-1] = (name, n)

    def framework_subs(self, stage: StageProtocol) -> dict[str, str]:
        """Runtime-owned substitution vars. Stages must not assemble these themselves."""
        return {
            "name": stage.name,
            "model": self.client.model,
            "cwd": self.cwd,
            "base_ref": self.base_ref,
        }

    @staticmethod
    def setup_dirs(
        state_dir: pathlib.Path,
        artifact_dir: pathlib.Path,
        gremlin_id: str | None,
    ) -> None:
        state_dir.mkdir(parents=True, exist_ok=True)
        artifact_dir.mkdir(parents=True, exist_ok=True)
        sf = state_dir / "state.json"
        if gremlin_id and not sf.exists():
            write_state(state_dir, {"id": gremlin_id})

    def done_for(self, path: str) -> set[str]:
        return self.data.done_for(path)

    def mark_done(self, path: str, child_name: str) -> None:
        self.data.mark_done(path, child_name)

    def clear_done(self, path: str) -> None:
        self.data.clear_done(path)

    def record_bail(self, reason: str, *, kind: str = "other") -> None:
        self.data.write_bail_file(kind, reason)

    def record_stage_progress(
        self, name: str, sub_stage: object = None, *, parent_stage: str = ""
    ) -> None:
        self.data.set_stage(name, sub_stage, parent_stage=parent_stage)

    def make_runner(
        self,
        entry: StageProtocol,
        gremlin: Gremlin,
        scope: Sequence[StageProtocol] | None = None,
        *,
        record_stage: bool = True,
    ) -> Callable[[], Any]:
        base_state = self
        gremlin_id = self.data.gremlin_id
        attempt = f"{entry.name}-{secrets.token_hex(4)}" if gremlin_id else ""
        scope_list = list(scope) if scope is not None else []

        def _prepare() -> State:
            if record_stage:
                base_state.data.set_stage(
                    entry.name, parent_stage=base_state.parent_stage
                )
            # Sync the per-stage client to state.json so fleet listings
            # reflect the actual model in use for the current stage.
            if str(base_state.client) != base_state.data.client:
                base_state.data.patch(client=str(base_state.client))
            if attempt:
                if base_state.child_key:
                    base_state.data.patch_parallel_attempt(
                        base_state.child_key, attempt
                    )
                else:
                    base_state.data.patch(attempt=attempt)
            fresh_handle = StateData(gremlin_id)
            return dataclasses.replace(
                base_state, data=fresh_handle, current_scope=scope_list
            )

        async def _run_async() -> Any:
            skip = entry.skip_if_exists
            if skip:
                skip = skip.replace("{loop_iter}", base_state.loop_iter)
                if base_state.artifacts.is_live(skip):
                    logger.info("stage skipped (artifact exists): %s", entry.name)
                    return Done()
            child_gremlin = copy.copy(gremlin)
            logger.debug("preparing state for stage: %s", entry.name)
            prepared_state = _prepare()
            child_gremlin.state = prepared_state
            child_gremlin.registry = prepared_state.artifacts
            logger.info("stage starting: %s (type=%s)", entry.name, entry.type)
            try:
                result = await entry.run(child_gremlin)
                return result
            finally:
                logger.info("stage finished: %s", entry.name)
                for h in logging.getLogger().handlers:
                    try:
                        h.flush()
                    except Exception:
                        pass

        return _run_async


def build_state(
    data: StateData,
    client: Client,
    artifact_dir: pathlib.Path,
    *,
    args: argparse.Namespace | None = None,
    pipeline_data: Pipeline | None = None,
    cwd: str = "",
    worktree: pathlib.Path | None = None,
    worktree_parent: pathlib.Path | None = None,
    artifacts: ArtifactRegistry | None = None,
    child_key: str | None = None,
    parent_stage: str = "",
    base_ref: str = "",
) -> State:
    reg = ArtifactRegistry(artifact_dir=artifact_dir)
    return State(
        data=data,
        client=client,
        artifact_dir=artifact_dir,
        artifacts=artifacts or reg,
        cwd=cwd or (str(worktree) if worktree is not None else str(project_root())),
        args=args if args is not None else argparse.Namespace(),
        pipeline_data=pipeline_data,
        worktree=worktree,
        worktree_parent=worktree_parent,
        child_key=child_key,
        parent_stage=parent_stage,
        base_ref=base_ref,
    )
