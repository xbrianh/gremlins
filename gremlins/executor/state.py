"""Execution context and state.json I/O for gremlin pipelines."""

from __future__ import annotations

import argparse
import dataclasses
import datetime
import json
import logging
import os
import pathlib
import re
import secrets
from collections.abc import Callable
from typing import TYPE_CHECKING, Any, cast

from gremlins import paths as _paths
from gremlins.clients.client import Client
from gremlins.utils.state_file import locked_update

if TYPE_CHECKING:
    from gremlins.pipeline import Pipeline
    from gremlins.stages.base import Stage

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


def resolve_session_dir(gremlin_id: str | None = None) -> pathlib.Path:
    """Resolve the artifacts directory for the current run."""
    state_root = _paths.state_root()
    if gremlin_id:
        session_dir = state_root / gremlin_id / "artifacts"
    else:
        ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        rand = secrets.token_hex(3)
        session_dir = state_root / "direct" / f"{ts}-{rand}" / "artifacts"
    session_dir.mkdir(parents=True, exist_ok=True)
    return session_dir


def write_state(state_dir: pathlib.Path, data: dict[str, Any]) -> None:
    """Atomically overwrite state.json (no merge)."""
    sf = state_dir / "state.json"
    tmp = state_dir / f"state.json.{os.getpid()}.{secrets.token_hex(4)}.tmp"
    tmp.write_text(json.dumps(data), encoding="utf-8")
    os.replace(tmp, sf)


def landable_shape(state: dict[str, Any]) -> str:
    """Classify artifact shape for land dispatch."""
    artifacts = list(state.get("artifacts") or [])
    branches: dict[str, bool] = {}
    prs: list[dict[str, Any]] = []

    for art in artifacts:
        if art.get("type") == "branch":
            name = str(art.get("name") or "")
            if name and name not in branches:
                branches[name] = False
        elif art.get("type") == "pr":
            prs.append(art)
            branch = str(art.get("branch") or "")
            if branch in branches:
                branches[branch] = True

    unmerged = [n for n, has_pr in branches.items() if not has_pr]

    if not prs and not unmerged:
        return "empty"
    if not prs and len(unmerged) == 1:
        return "one_branch"
    if not prs:
        return "many_branches"
    if unmerged:
        return "many_branches"
    if len(prs) == 1:
        return "one_pr"
    return "many_prs"


def _stage_list() -> list[Stage]:
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
    issue_url: str = ""
    base_ref_name: str = ""
    issue_num: str = ""
    loop_iteration: int = 1
    attempt: str = ""
    base_ref_sha: str = ""
    kind: str = ""
    project_root: str = ""
    workdir: str = ""
    setup_kind: str = ""
    worktree_base: str = ""
    status: str = ""
    started_at: str = ""
    instructions: str = ""
    description: str = ""
    description_explicit: bool = False
    parent_id: str = ""
    pipeline_args: list[str] = dataclasses.field(default_factory=list[str])
    client: str = ""
    pipeline_path: str = ""
    stage: str = ""
    pid: int | None = None
    stage_inputs: dict[str, Any] = dataclasses.field(default_factory=dict[str, Any])

    @classmethod
    def load(cls, gremlin_id: str | None) -> StateData:
        sf = resolve_state_file(gremlin_id)
        sd = _read_state_json(sf)
        return cls(
            gremlin_id=gremlin_id,
            state_file=sf,
            issue_url=sd.get("issue_url") or "",
            base_ref_name=sd.get("base_ref_name") or "",
            issue_num=sd.get("issue_num") or "",
            loop_iteration=_int_or(sd.get("loop_iteration"), 1),
            attempt=sd.get("attempt") or "",
            base_ref_sha=sd.get("base_ref_sha") or "",
            kind=sd.get("kind") or "",
            project_root=sd.get("project_root") or "",
            workdir=sd.get("workdir") or "",
            setup_kind=sd.get("setup_kind") or "",
            worktree_base=sd.get("worktree_base") or "",
            status=sd.get("status") or "",
            started_at=sd.get("started_at") or "",
            instructions=sd.get("instructions") or "",
            description=sd.get("description") or "",
            description_explicit=bool(sd.get("description_explicit")),
            parent_id=sd.get("parent_id") or "",
            pipeline_args=list(cast(list[str], sd.get("pipeline_args") or [])),
            client=sd.get("client") or "",
            pipeline_path=sd.get("pipeline_path") or "",
            stage=sd.get("stage") or "",
            pid=sd.get("pid"),
            stage_inputs=dict(cast(dict[str, Any], sd.get("stage_inputs") or {})),
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
            "instructions": self.instructions,
            "description": self.description,
            "description_explicit": self.description_explicit,
            "parent_id": self.parent_id,
            "pipeline_args": self.pipeline_args,
            "client": self.client,
            "pipeline_path": self.pipeline_path,
            "stage": self.stage,
            "pid": self.pid,
            "stage_inputs": self.stage_inputs,
            "base_ref_name": self.base_ref_name,
            "base_ref_sha": self.base_ref_sha,
            "issue_url": self.issue_url,
            "issue_num": self.issue_num,
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

    def check_bail(self, label: str = "stage", *, child_key: str | None = None) -> None:
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists():
            return
        try:
            data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
            if child_key is None:
                attempt = data.get("attempt") or ""
            else:
                pa: dict[str, Any] = data.get("parallel_attempts") or {}
                attempt = pa.get(child_key) or ""
            if attempt and (sf.parent / f"bail_{attempt}.json").exists():
                raise RuntimeError(f"{label} bailed (see bail_{attempt}.json)")
        except RuntimeError:
            raise
        except Exception:
            pass

    def append_artifact(self, artifact: dict[str, Any]) -> None:
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists():
            return
        try:

            def _apply(data: dict[str, Any]) -> None:
                arts: list[Any] = list(data.get("artifacts") or [])
                arts.append(artifact)
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

    def read_pr_url(self) -> str:
        for art in reversed(self.read_artifacts()):
            if art.get("type") == "pr":
                return str(art.get("url") or "")
        return ""

    def last_pr_branch(self) -> str:
        for art in reversed(self.read_artifacts()):
            if art.get("type") == "pr":
                return str(art.get("branch") or "")
        return ""

    def read_pr_num(self) -> str:
        url = self.read_pr_url()
        return url.split("/")[-1] if url else ""

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

    def patch_parallel_done(
        self,
        group_name: str,
        child_key: str | None = None,
    ) -> None:
        """Mark child done (child_key given) or clear the group (child_key=None)."""
        if not self.gremlin_id or not group_name:
            return
        sf = self.state_file or resolve_state_file(self.gremlin_id)
        if sf is None or not sf.exists():
            return
        try:

            def _apply(data: dict[str, Any]) -> None:
                groups: dict[str, Any] = dict(data.get("parallel_done") or {})
                if child_key is None:
                    groups.pop(group_name, None)
                else:
                    group: dict[str, str] = dict(groups.get(group_name) or {})
                    group[child_key] = "1"
                    groups[group_name] = group
                if groups:
                    data["parallel_done"] = groups
                else:
                    data.pop("parallel_done", None)

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
    session_dir: pathlib.Path
    test_client: Client | None = None
    args: argparse.Namespace = dataclasses.field(default_factory=argparse.Namespace)
    pipeline_data: Pipeline | None = None
    repo: str = ""
    instructions: str = ""
    current_scope: list[Stage] = dataclasses.field(default_factory=_stage_list)
    child_key: str | None = None
    parent_stage: str = ""
    worktree: pathlib.Path | None = None
    worktree_parent: pathlib.Path | None = None

    @staticmethod
    def setup_dirs(
        state_dir: pathlib.Path,
        session_dir: pathlib.Path,
        gremlin_id: str | None,
        *,
        instructions: str = "",
    ) -> None:
        state_dir.mkdir(parents=True, exist_ok=True)
        session_dir.mkdir(parents=True, exist_ok=True)
        (state_dir / "instructions.txt").write_text(instructions, encoding="utf-8")
        sf = state_dir / "state.json"
        if gremlin_id and not sf.exists():
            write_state(state_dir, {"id": gremlin_id})

    @property
    def cwd(self) -> pathlib.Path:
        return self.worktree if self.worktree is not None else pathlib.Path.cwd()

    def make_runner(
        self,
        entry: Stage,
        scope: list[Stage] | None = None,
    ) -> Callable[[], None]:
        base_state = self
        gremlin_id = self.data.gremlin_id
        attempt = f"{entry.name}-{secrets.token_hex(4)}" if gremlin_id else ""
        scope_list = list(scope) if scope is not None else []

        def _run() -> None:
            base_state.data.set_stage(entry.name, parent_stage=base_state.parent_stage)
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
            state = dataclasses.replace(
                base_state,
                data=loaded,
                current_scope=scope_list,
            )
            entry.run(state)

        return _run
