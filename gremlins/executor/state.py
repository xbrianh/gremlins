"""Execution context and state.json I/O for gremlin pipelines."""

from __future__ import annotations

import argparse
import dataclasses
import datetime
import json
import logging
import math
import os
import pathlib
import re
import secrets
from collections.abc import Callable, Sequence
from typing import TYPE_CHECKING, Any, ClassVar, cast

from gremlins import paths as _paths
from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.clients.client import Client
from gremlins.utils.state_file import locked_update

if TYPE_CHECKING:
    from gremlins.executor.gremlin import Gremlin
    from gremlins.pipeline import Pipeline

from gremlins.protocols import StageProtocol
from gremlins.stages.outcome import Done

logger = logging.getLogger(__name__)

_GREMLIN_ID_RE = re.compile(r"^[A-Za-z0-9_-]+$")

BAIL_CLASS_REVIEWER_REQUESTED_CHANGES = "reviewer_requested_changes"
BAIL_CLASS_SECURITY = "security"
BAIL_CLASS_SECRETS = "secrets"
BAIL_CLASS_OTHER = "other"


def validate_gremlin_id(gremlin_id: str) -> None:
    """Raise ValueError if gremlin_id is not a safe, non-path-traversing identifier."""
    if ".." in gremlin_id or not _GREMLIN_ID_RE.match(gremlin_id):
        raise ValueError(f"gremlin_id contains illegal characters: {gremlin_id!r}")


def resolve_state_file(gremlin_id: str | None) -> pathlib.Path | None:
    """Return path to state.json for gremlin_id, or None when gremlin_id is absent."""
    if not gremlin_id:
        return None
    return _paths.state_root() / gremlin_id / "state.json"


def resolve_artifact_dir(gremlin_id: str | None = None) -> pathlib.Path:
    """Resolve the artifacts directory for the current run."""
    state_root = _paths.state_root()
    if gremlin_id:
        artifact_dir = state_root / gremlin_id / "artifacts"
    else:
        ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        rand = secrets.token_hex(3)
        artifact_dir = state_root / "direct" / f"{ts}-{rand}" / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    return artifact_dir


def write_state(state_dir: pathlib.Path, data: dict[str, Any]) -> None:
    """Atomically overwrite state.json (no merge)."""
    sf = state_dir / "state.json"
    tmp = state_dir / f"state.json.{os.getpid()}.{secrets.token_hex(4)}.tmp"
    tmp.write_text(json.dumps(data), encoding="utf-8")
    os.replace(tmp, sf)


def landable_shape(state: dict[str, Any]) -> str:
    """Classify artifact shape for land dispatch."""
    artifacts = list(state.get("artifacts") or [])
    prs = [art for art in artifacts if art.get("type") == "pr"]

    if not prs:
        return "empty"
    if len(prs) == 1:
        return "one_pr"
    return "many_prs"


def _stage_list() -> list[StageProtocol]:
    return []


def _int_or(value: Any, default: int) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def _read_state_json(sf: pathlib.Path | None) -> dict[str, Any]:
    if sf is None or not sf.exists():
        return {}
    try:
        return json.loads(sf.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return {}


@dataclasses.dataclass
class StateData:
    gremlin_id: str | None = None
    state_file: pathlib.Path | None = None
    loop_iteration: int = 1
    attempt: str = ""
    kind: str = ""
    project_root: str = ""
    workdir: str = ""
    setup_kind: str = ""
    worktree_base: str = ""
    status: str = ""
    started_at: str = ""
    description: str = ""
    description_explicit: bool = False
    parent_id: str = ""
    pipeline_args: list[str] = dataclasses.field(default_factory=list[str])
    client: str = ""
    pipeline_path: str = ""
    stage: str = ""
    pid: int | None = None
    stage_inputs: dict[str, Any] = dataclasses.field(default_factory=dict[str, Any])
    bypass: bool = False
    permissions_file: str = ""
    group_name: str = ""
    child_key: str = ""
    exit_code: int | None = None

    @classmethod
    def load(cls, gremlin_id: str | None) -> StateData:
        sf = resolve_state_file(gremlin_id)
        sd = _read_state_json(sf)
        return cls(
            gremlin_id=gremlin_id,
            state_file=sf,
            loop_iteration=_int_or(sd.get("loop_iteration"), 1),
            attempt=sd.get("attempt") or "",
            kind=sd.get("kind") or "",
            project_root=sd.get("project_root") or "",
            workdir=sd.get("workdir") or "",
            setup_kind=sd.get("setup_kind") or "",
            worktree_base=sd.get("worktree_base") or "",
            status=sd.get("status") or "",
            started_at=sd.get("started_at") or "",
            description=sd.get("description") or "",
            description_explicit=bool(sd.get("description_explicit")),
            parent_id=sd.get("parent_id") or "",
            pipeline_args=list(cast(list[str], sd.get("pipeline_args") or [])),
            client=sd.get("client") or "",
            pipeline_path=sd.get("pipeline_path") or "",
            stage=sd.get("stage") or "",
            pid=sd.get("pid"),
            stage_inputs=dict(cast(dict[str, Any], sd.get("stage_inputs") or {})),
            bypass=bool(sd.get("bypass", False)),
            permissions_file=sd.get("permissions_file") or "",
            group_name=sd.get("group_name") or "",
            child_key=sd.get("child_key") or "",
            exit_code=int(sd["exit_code"]) if sd.get("exit_code") is not None else None,
        )

    def persist(self, state_dir: pathlib.Path) -> None:
        if not self.gremlin_id:
            raise ValueError("cannot persist StateData with no gremlin_id")
        data: dict[str, Any] = {
            "id": self.gremlin_id,
            "loop_iteration": self.loop_iteration,
            "attempt": self.attempt,
            "kind": self.kind,
            "project_root": self.project_root,
            "workdir": self.workdir,
            "setup_kind": self.setup_kind,
            "worktree_base": self.worktree_base,
            "status": self.status,
            "started_at": self.started_at,
            "description": self.description,
            "description_explicit": self.description_explicit,
            "parent_id": self.parent_id,
            "pipeline_args": self.pipeline_args,
            "client": self.client,
            "pipeline_path": self.pipeline_path,
            "stage": self.stage,
            "pid": self.pid,
            "stage_inputs": self.stage_inputs,
            "bypass": self.bypass,
            "permissions_file": self.permissions_file,
            "group_name": self.group_name,
            "child_key": self.child_key,
            "exit_code": self.exit_code,
        }
        write_state(state_dir, data)
        self.state_file = state_dir / "state.json"

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

    def write_bail_file(
        self, bail_class: str, bail_detail: str = "", *, attempt: str = ""
    ) -> None:
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists() or not attempt or not bail_class:
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

    def append_artifact(self, artifact: dict[str, Any]) -> None:
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists():
            return
        stamped = {**artifact, "attempt": self.attempt} if self.attempt else artifact
        try:

            def _apply(data: dict[str, Any]) -> None:
                arts: list[Any] = list(data.get("artifacts") or [])
                arts.append(stamped)
                data["artifacts"] = arts

            locked_update(sf, _apply)
        except Exception:
            logger.warning("failed to append artifact", exc_info=True)

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

    def read_artifacts(self) -> list[dict[str, Any]]:
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists():
            return []
        try:
            data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
            artifacts: list[Any] = data.get("artifacts") or []
            return [a for a in artifacts if isinstance(a, dict)]
        except (json.JSONDecodeError, OSError):
            return []

    def read_artifacts_for_attempt(self, attempt: str) -> list[dict[str, Any]]:
        if not attempt:
            return []
        return [a for a in self.read_artifacts() if a.get("attempt") == attempt]

    def last_artifact_branch(self) -> str:
        for art in reversed(self.read_artifacts()):
            if art.get("type") == "branch":
                return str(art.get("name") or "")
            if art.get("type") == "pr":
                return str(art.get("branch") or "")
        return ""

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
    repo: str = ""
    cwd: str = ""
    args: argparse.Namespace = dataclasses.field(default_factory=argparse.Namespace)
    pipeline_data: Pipeline | None = None
    current_scope: list[StageProtocol] = dataclasses.field(default_factory=_stage_list)
    child_key: str | None = None
    parent_stage: str = ""
    worktree: pathlib.Path | None = None
    worktree_parent: pathlib.Path | None = None
    base_ref: str = ""

    FRAMEWORK_KEYS: ClassVar[frozenset[str]] = frozenset(
        {
            "name",
            "model",
            "artifact_dir",
            "repo",
            "cwd",
            "base_ref",
            "loop_iteration",
        }
    )

    def framework_subs(self, stage: StageProtocol) -> dict[str, str]:
        """Runtime-owned substitution vars. Stages must not assemble these themselves."""
        return {
            "name": stage.name,
            "model": self.client.model,
            "artifact_dir": str(self.artifact_dir),
            "repo": self.repo,
            "cwd": self.cwd,
            "base_ref": self.base_ref,
            "loop_iteration": str(self.data.loop_iteration),
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

    def format(self, template: str) -> str:
        scope = "/".join(s.name for s in self.current_scope)
        return template.format(
            n=self.data.loop_iteration,
            attempt=self.data.attempt,
            scope=scope,
            repo=self.repo,
            cwd=self.cwd,
            base_ref=self.base_ref,
        )

    def done_for(self, path: str) -> set[str]:
        return self.data.done_for(path)

    def mark_done(self, path: str, child_name: str) -> None:
        self.data.mark_done(path, child_name)

    def clear_done(self, path: str) -> None:
        self.data.clear_done(path)

    def record_bail(self, reason: str, *, kind: str = "other") -> None:
        self.data.write_bail_file(kind, reason, attempt=self.data.attempt)

    def record_stage_progress(
        self, name: str, sub_stage: object = None, *, parent_stage: str = ""
    ) -> None:
        self.data.set_stage(name, sub_stage, parent_stage=parent_stage)

    def record_state_field(self, **fields: Any) -> None:
        self.data.patch(**fields)

    def make_runner(
        self,
        entry: StageProtocol,
        scope: Sequence[StageProtocol] | None = None,
        *,
        record_stage: bool = True,
        gremlin: Gremlin,
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
            if attempt:
                if base_state.child_key:
                    base_state.data.patch_parallel_attempt(
                        base_state.child_key, attempt
                    )
                else:
                    base_state.data.patch(attempt=attempt)
            loaded = StateData.load(gremlin_id)
            if attempt:
                loaded = dataclasses.replace(loaded, attempt=attempt)
            return dataclasses.replace(
                base_state, data=loaded, current_scope=scope_list
            )

        async def _run_async() -> Any:
            if entry.skip_if_exists and base_state.artifacts.verified(
                entry.skip_if_exists
            ):
                return Done()
            prepared = _prepare()
            gremlin.state = prepared
            return await entry.run(gremlin)

        return _run_async


def build_state(
    data: StateData,
    client: Client,
    artifact_dir: pathlib.Path,
    *,
    args: argparse.Namespace | None = None,
    pipeline_data: Pipeline | None = None,
    repo: str = "",
    cwd: str = "",
    worktree: pathlib.Path | None = None,
    worktree_parent: pathlib.Path | None = None,
    artifacts: ArtifactRegistry | None = None,
    child_key: str | None = None,
    parent_stage: str = "",
    base_ref: str = "",
) -> State:
    reg = ArtifactRegistry(artifact_dir=artifact_dir, cwd=worktree)
    return State(
        data=data,
        client=client,
        artifact_dir=artifact_dir,
        artifacts=artifacts or reg,
        repo=repo,
        cwd=cwd
        or (str(worktree) if worktree is not None else str(_paths.project_root())),
        args=args if args is not None else argparse.Namespace(),
        pipeline_data=pipeline_data,
        worktree=worktree,
        worktree_parent=worktree_parent,
        child_key=child_key,
        parent_stage=parent_stage,
        base_ref=base_ref,
    )
