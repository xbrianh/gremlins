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
from typing import TYPE_CHECKING, Any

from gremlins import paths as _paths
from gremlins.clients.client import Client
from gremlins.utils.state_file import locked_update

if TYPE_CHECKING:
    from gremlins.pipeline import Pipeline
    from gremlins.stages.base import Stage

logger = logging.getLogger(__name__)

_GR_ID_RE = re.compile(r"^[A-Za-z0-9_-]+$")

BAIL_CLASS_REVIEWER_REQUESTED_CHANGES = "reviewer_requested_changes"
BAIL_CLASS_SECURITY = "security"
BAIL_CLASS_SECRETS = "secrets"
BAIL_CLASS_OTHER = "other"


def validate_gr_id(gr_id: str) -> None:
    """Raise ValueError if gr_id is not a safe, non-path-traversing identifier."""
    if ".." in gr_id or not _GR_ID_RE.match(gr_id):
        raise ValueError(f"gr_id contains illegal characters: {gr_id!r}")


def resolve_state_file(gr_id: str | None) -> pathlib.Path | None:
    """Return path to state.json for gr_id, or None when gr_id is absent."""
    if not gr_id:
        return None
    return _paths.state_root() / gr_id / "state.json"


def resolve_session_dir(gr_id: str | None = None) -> pathlib.Path:
    """Resolve the artifacts directory for the current run."""
    state_root = _paths.state_root()
    if gr_id:
        session_dir = state_root / gr_id / "artifacts"
    else:
        ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        rand = secrets.token_hex(3)
        session_dir = state_root / "direct" / f"{ts}-{rand}" / "artifacts"
    session_dir.mkdir(parents=True, exist_ok=True)
    return session_dir


def set_stage(
    gr_id: str | None,
    stage: str,
    sub_stage: object = None,
) -> None:
    """Write stage and optional sub-stage to state.json."""
    try:
        if not stage or not gr_id:
            return
        sf = resolve_state_file(gr_id)
        if sf is None or not sf.exists():
            return
        now = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
        if sub_stage is not None:
            patch_state(gr_id, stage=stage, stage_updated_at=now, sub_stage=sub_stage)
        else:
            patch_state(
                gr_id, _delete=("sub_stage",), stage=stage, stage_updated_at=now
            )
    except Exception:
        pass


def write_bail_file(
    gr_id: str | None,
    attempt: str,
    bail_class: str,
    bail_detail: str = "",
) -> None:
    """Write bail_{attempt}.json atomically to state dir. No-op if attempt or gr_id empty."""
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists() or not attempt or not bail_class:
        return
    try:
        state_dir = sf.parent
        bail_path = state_dir / f"bail_{attempt}.json"
        if bail_path.exists():
            return  # idempotent
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


def read_bail_info(gr_id: str | None) -> dict[str, str] | None:
    """Return bail file contents for current attempt, or None if no bail."""
    sf = resolve_state_file(gr_id)
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


def read_state_str(state_file: pathlib.Path | None, field: str) -> str:
    """Return a string field from state.json, or '' on missing file or error."""
    if state_file is None or not state_file.exists():
        return ""
    try:
        return json.loads(state_file.read_text(encoding="utf-8")).get(field) or ""
    except Exception:
        return ""


def write_state(state_dir: pathlib.Path, data: dict[str, Any]) -> None:
    """Atomically overwrite state.json (no merge)."""
    sf = state_dir / "state.json"
    tmp = state_dir / f"state.json.{os.getpid()}.{secrets.token_hex(4)}.tmp"
    tmp.write_text(json.dumps(data), encoding="utf-8")
    os.replace(tmp, sf)


def patch_state(
    gr_id: str | None, _delete: tuple[str, ...] = (), **fields: object
) -> None:
    """Merge keyword fields into state.json atomically under an exclusive file lock."""
    sf = resolve_state_file(gr_id)
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


def patch_parallel_worktrees(
    gr_id: str | None,
    group_name: str,
    *,
    base_head: str | None,
    paths: dict[str, str] | None,
) -> None:
    """Set or clear ``parallel_worktrees[group_name]`` in state.json."""
    if not gr_id or not group_name:
        return
    sf = resolve_state_file(gr_id)
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


def read_pr_url(gr_id: str | None) -> str:
    for art in reversed(read_artifacts(gr_id)):
        if art.get("type") == "pr":
            return str(art.get("url") or "")
    return ""


def last_pr_branch(gr_id: str | None) -> str:
    for art in reversed(read_artifacts(gr_id)):
        if art.get("type") == "pr":
            return str(art.get("branch") or "")
    return ""


def read_pr_num(gr_id: str | None) -> str:
    url = read_pr_url(gr_id)
    return url.split("/")[-1] if url else ""


def append_artifact(gr_id: str | None, artifact: dict[str, Any]) -> None:
    sf = resolve_state_file(gr_id)
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


def read_artifacts(gr_id: str | None) -> list[dict[str, Any]]:
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return []
    try:
        data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
        artifacts: list[Any] = data.get("artifacts") or []
        return [a for a in artifacts if isinstance(a, dict)]
    except (json.JSONDecodeError, OSError):
        return []


def last_artifact_branch(gr_id: str | None) -> str:
    for art in reversed(read_artifacts(gr_id)):
        if art.get("type") == "branch":
            return str(art.get("name") or "")
        if art.get("type") == "pr":
            return str(art.get("branch") or "")
    return ""


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


def check_bail(
    gr_id: str | None,
    label: str = "stage",
    *,
    child_key: str | None = None,
) -> None:
    """Raise RuntimeError if bail_{attempt}.json exists in state dir."""
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return
    try:
        data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
        if child_key is None:
            attempt = data.get("attempt") or ""
        else:
            pa: dict[str, Any] = data.get("parallel_attempts") or {}
            attempt: str = pa.get(child_key) or ""
        if attempt and (sf.parent / f"bail_{attempt}.json").exists():
            raise RuntimeError(f"{label} bailed (see bail_{attempt}.json)")
    except RuntimeError:
        raise
    except Exception:
        pass


def _patch_parallel_attempt(gr_id: str | None, child_key: str, attempt: str) -> None:
    """Write parallel_attempts[child_key] = attempt into state.json."""
    sf = resolve_state_file(gr_id)
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
class State:
    # required per-stage
    client: Client
    session_dir: pathlib.Path
    # pipeline-wide (all have defaults so tests can omit them)
    gr_id: str | None = None
    state_file: pathlib.Path | None = None
    args: argparse.Namespace = dataclasses.field(default_factory=argparse.Namespace)
    pipeline_data: Pipeline | None = None
    repo: str = ""
    instructions: str = ""
    test_client: Client | None = None
    # per-stage optional
    child_key: str | None = None
    parent_stage: str = ""
    worktree: pathlib.Path | None = None
    current_scope: list[Stage] = dataclasses.field(default_factory=_stage_list)
    # runtime-derived (populated from state.json before each stage run)
    issue_url: str = ""
    base_ref_name: str = ""
    issue_num: str = ""
    loop_iteration: int = 1
    attempt: str = ""

    @staticmethod
    def setup_dirs(
        state_dir: pathlib.Path,
        session_dir: pathlib.Path,
        gr_id: str | None,
        *,
        instructions: str = "",
    ) -> None:
        state_dir.mkdir(parents=True, exist_ok=True)
        session_dir.mkdir(parents=True, exist_ok=True)
        (state_dir / "instructions.txt").write_text(instructions, encoding="utf-8")
        sf = state_dir / "state.json"
        if gr_id and not sf.exists():
            # bootstraps state.json when called outside the launcher
            write_state(state_dir, {"id": gr_id})

    @property
    def cwd(self) -> pathlib.Path:
        return self.worktree if self.worktree is not None else pathlib.Path.cwd()

    def make_runner(
        self,
        entry: Stage,
        scope: list[Stage] | None = None,
    ) -> Callable[[], None]:
        base_state = self
        gr_id = self.gr_id
        attempt = f"{entry.name}-{secrets.token_hex(4)}" if gr_id else ""
        scope_list = list(scope) if scope is not None else []

        def _run() -> None:
            base_state.set_stage(entry.name)
            if attempt:
                if base_state.child_key:
                    _patch_parallel_attempt(gr_id, base_state.child_key, attempt)
                else:
                    patch_state(gr_id, attempt=attempt)
            sf = (
                base_state.state_file
                if base_state.state_file is not None
                else resolve_state_file(gr_id)
            )
            sd = _read_state_json(sf)
            state: State = dataclasses.replace(
                base_state,
                current_scope=scope_list,
                attempt=attempt,
                issue_url=sd.get("issue_url") or "",
                base_ref_name=sd.get("base_ref_name") or "",
                issue_num=sd.get("issue_num") or "",
                loop_iteration=_int_or(sd.get("loop_iteration"), 1),
            )
            entry.run(state)

        return _run

    # --- state.json I/O methods ---

    def patch(self, _delete: tuple[str, ...] = (), **fields: object) -> None:
        patch_state(self.gr_id, _delete=_delete, **fields)

    def read_str(self, field: str) -> str:
        return read_state_str(self.state_file, field)

    def set_stage(self, stage: str, sub_stage: object = None) -> None:
        if self.parent_stage:
            set_stage(self.gr_id, self.parent_stage, sub_stage=stage)
        else:
            set_stage(self.gr_id, stage, sub_stage)

    def write_bail_file(self, bail_class: str, bail_detail: str = "") -> None:
        write_bail_file(self.gr_id, self.attempt, bail_class, bail_detail)

    def check_bail(self, label: str = "stage") -> None:
        check_bail(self.gr_id, label, child_key=self.child_key)

    def append_artifact(self, artifact: dict[str, Any]) -> None:
        append_artifact(self.gr_id, artifact)

    def read_pr_url(self) -> str:
        return read_pr_url(self.gr_id)

    def last_pr_branch(self) -> str:
        return last_pr_branch(self.gr_id)

    def read_pr_num(self) -> str:
        return read_pr_num(self.gr_id)


def write_terminal_state(gr_id: str, exit_code: int) -> None:
    """Record terminal outcome for a finished pipeline run."""
    state_dir = _paths.state_root() / gr_id
    try:
        (state_dir / "finished").touch()
    except OSError:
        pass
    now_iso = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
    status = "done" if exit_code == 0 else "stopped"
    try:
        patch_state(gr_id, status=status, ended_at=now_iso, exit_code=exit_code)
    except Exception:
        pass
